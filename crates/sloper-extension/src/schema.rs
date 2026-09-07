//! Compile-time schemas and declaration encoding shared with the derives.

/// The bounded schema of one serializable value.
///
/// Derive this trait for user types. Containers are spelled at the field use
/// site so the derive can inspect optionality and reject ambiguous aliases.
/// String formats use `#[schema(format = "email")]`; arrays use
/// `#[schema(items(format = "email"))]`. Supported formats are `date`,
/// `date-time`, `email`, `uri`, and `uuid`. Serde preserves the original
/// string; the host validates the declared format at the capability boundary.
///
/// ```
/// use sloper_extension::Schema;
///
/// #[derive(Schema)]
/// struct Contact {
///     #[schema(format = "email")]
///     email: String,
///     #[schema(items(format = "uri"))]
///     links: Vec<String>,
/// }
/// ```
pub trait Schema {
    /// Non-null JSON type name.
    const TYPE: &'static str;
    /// JSON members following `type`, including the leading comma when present.
    const REST: &'static str;
    /// Whether any nested property contains a managed source.
    const HAS_SOURCE: bool;
    /// Complete JSON schema in declaration order.
    const JSON: &'static str;
    /// Whether this schema or a nested object accepts additional fields.
    #[doc(hidden)]
    const OPEN: bool = false;
}

/// An item type declared by an extension resource.
pub trait Resource: Schema + serde::Serialize + serde::de::DeserializeOwned {
    /// Manifest resource name.
    const NAME: &'static str;
    /// Serialized key property, required by writable resources.
    const KEY: Option<&'static str>;
    /// Named resource declaration used by manifest assembly.
    #[doc(hidden)]
    const PART: &'static str;
}

/// A named OAuth profile and its requested scopes.
pub trait ConnectionType {
    /// Manifest connection name.
    const NAME: &'static str;
    /// Supported OAuth profile name.
    const PROFILE: &'static str;
    /// Non-empty, unique requested scopes.
    const SCOPES: &'static [&'static str];
    /// Named connection declaration used by manifest assembly.
    #[doc(hidden)]
    const PART: &'static str;
}

/// Explicit additional properties accepted by a `#[serde(flatten)]` field.
pub type Fields = std::collections::BTreeMap<String, serde_json::Value>;

macro_rules! scalar_schema {
    ($($ty:ty => ($kind:literal, $rest:expr)),* $(,)?) => {
        $(impl Schema for $ty {
            const TYPE: &'static str = $kind;
            const REST: &'static str = $rest;
            const HAS_SOURCE: bool = false;
            const JSON: &'static str = concat!("{\"type\":\"", $kind, "\"", $rest, "}");
        })*
    };
}
scalar_schema! {
    String => ("string", ""),
    bool => ("boolean", ""),
    f64 => ("number", ""),
    i32 => ("integer", ""),
    u32 => ("integer", ",\"minimum\":0"),
}

impl Schema for crate::Source {
    const HAS_SOURCE: bool = true;
    const JSON: &'static str = "{\"type\":\"string\",\"format\":\"source\"}";
    const REST: &'static str = ",\"format\":\"source\"";
    const TYPE: &'static str = "string";
}

// One manifest is bounded to 256 KiB. This fixed array avoids
// generic const expressions and nested constants capturing user parameters.
const MAX_DECLARATION_BYTES: usize = 256 * 1024;

#[doc(hidden)]
#[derive(Debug)]
pub struct SchemaBuffer {
    bytes: [u8; MAX_DECLARATION_BYTES],
    len: usize,
}

impl SchemaBuffer {
    #[must_use]
    #[allow(clippy::large_stack_arrays)]
    #[doc(hidden)]
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_DECLARATION_BYTES],
            len: 0,
        }
    }

    #[doc(hidden)]
    #[track_caller]
    pub const fn push(&mut self, text: &str) {
        let bytes = text.as_bytes();
        assert!(
            self.len + bytes.len() <= MAX_DECLARATION_BYTES,
            "Extension declarations exceed 256 KiB."
        );
        let mut index = 0;
        while index < bytes.len() {
            self.bytes[self.len] = bytes[index];
            self.len += 1;
            index += 1;
        }
    }

    #[doc(hidden)]
    pub const fn number(&mut self, mut value: usize) {
        let mut digits = [0; 20];
        let mut count = 0;
        loop {
            digits[count] = b"0123456789"[value % 10];
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            let digit = [digits[count]];
            let Ok(text) = core::str::from_utf8(&digit) else {
                unreachable!()
            };
            self.push(text);
        }
    }

    #[doc(hidden)]
    pub const fn quoted(&mut self, value: &str) {
        self.push("\"");
        let bytes = value.as_bytes();
        let mut start = 0;
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte == b'"' || byte == b'\\' || byte < 0x20 {
                let (prefix, _) = bytes.split_at(index);
                let (_, segment) = prefix.split_at(start);
                let Ok(text) = core::str::from_utf8(segment) else {
                    unreachable!()
                };
                self.push(text);
                if byte == b'"' {
                    self.push("\\\"");
                } else if byte == b'\\' {
                    self.push("\\\\");
                } else {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let escaped = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(byte / 16) as usize],
                        HEX[(byte % 16) as usize],
                    ];
                    let Ok(text) = core::str::from_utf8(&escaped) else {
                        unreachable!()
                    };
                    self.push(text);
                }
                start = index + 1;
            }
            index += 1;
        }
        let (_, tail) = bytes.split_at(start);
        let Ok(text) = core::str::from_utf8(tail) else {
            unreachable!()
        };
        self.push(text);
        self.push("\"");
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn as_str(&self) -> &str {
        let (bytes, _) = self.bytes.split_at(self.len);
        match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => unreachable!(),
        }
    }

    /// Merge use-site constraints without producing duplicate JSON keys.
    #[doc(hidden)]
    #[track_caller]
    pub const fn members(&mut self, inherited: &str, local: &str) {
        let mut offset = 0;
        while offset < inherited.len() {
            let end = member_end(inherited, offset);
            let member = substring(inherited, offset, end);
            let key = member_key(member);
            if let Some(replacement) = find_member(local, key) {
                self.push(refine_member(key, member, replacement));
            } else {
                self.push(member);
            }
            offset = end;
        }
        let mut offset = 0;
        while offset < local.len() {
            let end = member_end(local, offset);
            let member = substring(local, offset, end);
            if find_member(inherited, member_key(member)).is_none() {
                self.push(member);
            }
            offset = end;
        }
    }
}

const fn substring(value: &str, start: usize, end: usize) -> &str {
    let (prefix, _) = value.as_bytes().split_at(end);
    let (_, bytes) = prefix.split_at(start);
    let Ok(value) = core::str::from_utf8(bytes) else {
        unreachable!()
    };
    value
}

const fn member_end(value: &str, start: usize) -> usize {
    let bytes = value.as_bytes();
    let mut offset = start + 1;
    let mut depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    while offset < bytes.len() {
        let byte = bytes[offset];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'{' || byte == b'[' {
            depth += 1;
        } else if byte == b'}' || byte == b']' {
            depth -= 1;
        } else if byte == b',' && depth == 0 {
            return offset;
        }
        offset += 1;
    }
    offset
}

const fn member_key(member: &str) -> &str {
    let mut end = 2;
    while member.as_bytes()[end] != b'"' {
        end += 1;
    }
    substring(member, 2, end)
}

const fn member_value(member: &str) -> &str {
    substring(member, member_key(member).len() + 4, member.len())
}

const fn find_member<'a>(value: &'a str, key: &str) -> Option<&'a str> {
    let mut offset = 0;
    while offset < value.len() {
        let end = member_end(value, offset);
        let member = substring(value, offset, end);
        if equal(member_key(member), key) {
            return Some(member);
        }
        offset = end;
    }
    None
}

#[track_caller]
const fn refine_member<'a>(key: &str, inherited: &'a str, local: &'a str) -> &'a str {
    if equal(key, "title") || equal(key, "description") {
        return local;
    }
    let minimum = equal(key, "minimum") || equal(key, "minLength") || equal(key, "minItems");
    let maximum = equal(key, "maximum") || equal(key, "maxLength") || equal(key, "maxItems") || equal(key, "maxBytes");
    if minimum || maximum {
        let old = decimal(member_value(inherited));
        let new = decimal(member_value(local));
        return if (minimum && new > old) || (maximum && new < old) {
            local
        } else {
            inherited
        };
    }
    if equal(key, "uniqueItems") {
        return if equal(member_value(local), "true") {
            local
        } else {
            inherited
        };
    }
    assert!(
        equal(inherited, local),
        "Repeated schema constraints must agree; place media constraints on their source declaration."
    );
    inherited
}

const fn decimal(text: &str) -> f64 {
    let bytes = text.as_bytes();
    let negative = bytes[0] == b'-';
    let mut offset = if negative {
        1
    } else {
        0
    };
    let mut value = 0.0;
    let mut fraction = 1.0;
    let mut fractional = false;
    while offset < bytes.len() && bytes[offset] != b'e' && bytes[offset] != b'E' {
        if bytes[offset] == b'.' {
            fractional = true;
        } else {
            value = value * 10.0 + (bytes[offset] - b'0') as f64;
            if fractional {
                fraction *= 10.0;
            }
        }
        offset += 1;
    }
    value /= fraction;
    if offset < bytes.len() {
        offset += 1;
        let negative_exponent = bytes[offset] == b'-';
        if bytes[offset] == b'-' || bytes[offset] == b'+' {
            offset += 1;
        }
        let mut exponent = 0;
        while offset < bytes.len() {
            exponent = exponent * 10 + (bytes[offset] - b'0') as usize;
            offset += 1;
        }
        while exponent > 0 {
            if negative_exponent {
                value /= 10.0;
            } else {
                value *= 10.0;
            }
            exponent -= 1;
        }
    }
    if negative {
        -value
    } else {
        value
    }
}

impl Default for SchemaBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ResourceUse {
    pub name: &'static str,
    pub part: &'static str,
    pub write: bool,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ConnectionUse {
    pub name: &'static str,
    pub part: &'static str,
}

#[doc(hidden)]
#[derive(Debug)]
pub struct ActionParts {
    pub name: &'static str,
    pub resources: &'static [ResourceUse],
    pub connections: &'static [ConnectionUse],
}

#[doc(hidden)]
#[must_use]
pub const fn equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

#[doc(hidden)]
#[track_caller]
#[must_use]
pub const fn const_bytes<const N: usize>(value: &str) -> [u8; N] {
    assert!(N == value.len(), "Declaration byte length must match its static array.");
    let mut bytes = [0; N];
    let mut index = 0;
    while index < N {
        bytes[index] = value.as_bytes()[index];
        index += 1;
    }
    bytes
}

#[doc(hidden)]
#[track_caller]
#[must_use]
pub const fn action_part(parts: &ActionParts, description: Option<&str>, parameters: Option<&str>) -> SchemaBuffer {
    let mut output = SchemaBuffer::new();
    output.push("{\"kind\":\"action\",\"name\":");
    output.quoted(parts.name);
    if let Some(description) = description {
        output.push(",\"description\":");
        output.quoted(description);
    }
    let resources = resource_count(parts, parameters);
    if resources > 0 {
        output.push(",\"mode\":\"background\"");
    }
    if let Some(parameters) = parameters {
        output.push(",\"parameters\":");
        output.push(parameters);
    }
    if !parts.connections.is_empty() {
        output.push(",\"connections\":[");
        let mut index = 0;
        while index < parts.connections.len() {
            let mut prior = 0;
            while prior < index {
                assert!(
                    !equal(parts.connections[index].name, parts.connections[prior].name),
                    "Connection names cannot repeat within an action."
                );
                prior += 1;
            }
            if index > 0 {
                output.push(",");
            }
            output.quoted(parts.connections[index].name);
            index += 1;
        }
        output.push("]");
    }
    if resources > 0 {
        output.push(",\"resources\":{");
        let mut emitted = 0;
        let mut index = 0;
        while index < parts.resources.len() {
            let entry = &parts.resources[index];
            let mut prior = 0;
            let mut seen = false;
            while prior < index {
                seen |= equal(entry.name, parts.resources[prior].name);
                prior += 1;
            }
            if !seen {
                if emitted > 0 {
                    output.push(",");
                }
                output.quoted(entry.name);
                output.push(":[");
                let mut read = false;
                let mut write = false;
                let mut scan = index;
                while scan < parts.resources.len() {
                    if equal(entry.name, parts.resources[scan].name) {
                        read |= !parts.resources[scan].write;
                        write |= parts.resources[scan].write;
                    }
                    scan += 1;
                }
                if read {
                    output.push("\"read\"");
                }
                if read && write {
                    output.push(",");
                }
                if write {
                    output.push("\"write\"");
                }
                output.push("]");
                emitted += 1;
            }
            index += 1;
        }
        output.push("}");
    }
    output.push("}");
    output
}

#[track_caller]
const fn resource_count(parts: &ActionParts, parameters: Option<&str>) -> usize {
    let mut resources = 0;
    let mut has_reader = false;
    let mut has_writer = false;
    let mut index = 0;
    while index < parts.resources.len() {
        let entry = &parts.resources[index];
        has_reader |= !entry.write;
        has_writer |= entry.write;
        let mut prior = 0;
        let mut seen = false;
        while prior < index {
            let previous = &parts.resources[prior];
            if equal(entry.name, previous.name) {
                assert!(
                    entry.write != previous.write,
                    "Two readers or two writers cannot name the same resource."
                );
                assert!(
                    equal(entry.part, previous.part),
                    "Resource declarations with one name must be identical."
                );
                seen = true;
            }
            prior += 1;
        }
        if !seen {
            resources += 1;
        }
        index += 1;
    }
    assert!(resources <= 8, "An action may use at most eight resources.");
    assert!(
        !has_reader || parameters.is_none(),
        "Readers cannot be combined with owned parameters."
    );
    assert!(
        !has_writer || parts.connections.len() <= 1,
        "An action with a writer accepts at most one connection."
    );
    resources
}

#[doc(hidden)]
#[track_caller]
#[must_use]
#[allow(clippy::too_many_lines)]
pub const fn extension_parts(
    name: &str,
    version: &str,
    label: Option<&str>,
    description: &str,
    configuration: Option<&str>,
    actions: &[&ActionParts],
) -> SchemaBuffer {
    assert!(
        !description.is_empty() && description.len() <= 200,
        "Cargo package descriptions must be non-empty and fit 200 bytes."
    );
    assert!(
        !actions.is_empty() && actions.len() <= 64,
        "Declare between one and 64 extension actions."
    );
    let mut output = SchemaBuffer::new();
    output.push("{\"kind\":\"extension\",\"name\":");
    output.quoted(name);
    output.push(",\"version\":");
    output.quoted(version);
    output.push(",\"description\":");
    output.quoted(description);
    if let Some(label) = label {
        output.push(",\"label\":");
        output.quoted(label);
    }
    if let Some(configuration) = configuration {
        output.push(",\"configuration\":");
        output.push(configuration);
    }
    output.push(",\"actions\":[");
    let mut index = 0;
    while index < actions.len() {
        if index > 0 {
            output.push(",");
        }
        output.quoted(actions[index].name);
        index += 1;
    }
    output.push("]}");
    let mut action_index = 0;
    while action_index < actions.len() {
        let action = actions[action_index];
        let mut index = 0;
        while index < action.resources.len() {
            let current = &action.resources[index];
            let mut seen = false;
            let mut previous_action = 0;
            'previous_resources: while previous_action <= action_index {
                let previous = actions[previous_action];
                let limit = if previous_action == action_index {
                    index
                } else {
                    previous.resources.len()
                };
                let mut previous_index = 0;
                while previous_index < limit {
                    let candidate = &previous.resources[previous_index];
                    if equal(current.name, candidate.name) {
                        assert!(
                            equal(current.part, candidate.part),
                            "Conflicting resource declarations use the same name."
                        );
                        seen = true;
                        // Earlier occurrences already matched this first
                        // declaration, so one byte comparison is sufficient.
                        break 'previous_resources;
                    }
                    previous_index += 1;
                }
                previous_action += 1;
            }
            if !seen {
                output.push(current.part);
            }
            index += 1;
        }
        let mut index = 0;
        while index < action.connections.len() {
            let current = &action.connections[index];
            let mut seen = false;
            let mut previous_action = 0;
            'previous_connections: while previous_action <= action_index {
                let previous = actions[previous_action];
                let limit = if previous_action == action_index {
                    index
                } else {
                    previous.connections.len()
                };
                let mut previous_index = 0;
                while previous_index < limit {
                    let candidate = &previous.connections[previous_index];
                    if equal(current.name, candidate.name) {
                        assert!(
                            equal(current.part, candidate.part),
                            "Conflicting connection declarations use the same name."
                        );
                        seen = true;
                        break 'previous_connections;
                    }
                    previous_index += 1;
                }
                previous_action += 1;
            }
            if !seen {
                output.push(current.part);
            }
            index += 1;
        }
        action_index += 1;
    }
    output
}

#[cfg(test)]
mod tests {
    use serde::{
        Deserialize,
        Serialize,
    };

    use super::{
        ActionParts,
        ConnectionUse,
        ResourceUse,
        Schema,
        SchemaBuffer,
        extension_parts,
    };

    #[derive(Debug, Deserialize, Serialize, crate::Schema)]
    #[schema(crate = "crate")]
    struct FormattedStrings {
        #[schema(format = "date")]
        date: String,
        #[schema(format = "date-time")]
        timestamp: String,
        #[schema(format = "email")]
        email: Option<String>,
        #[schema(format = "uri")]
        uri: String,
        #[schema(format = "uuid")]
        uuid: String,
    }

    #[test]
    fn scalar_formats_preserve_original_wire_strings() -> serde_json::Result<()> {
        let wire = serde_json::json!({
            "date": "2024-02-29",
            "timestamp": "2024-02-29T03:04:05.123456789+02:00",
            "email": "First.Last+Tag@EXAMPLE.com",
            "uri": "https://EXAMPLE.com:443/a/../b?q=%2f",
            "uuid": "550E8400-E29B-41D4-A716-446655440000",
        });
        let value: FormattedStrings = serde_json::from_value(wire.clone())?;
        assert_eq!(serde_json::to_value(value)?, wire);

        let schema: serde_json::Value = serde_json::from_str(FormattedStrings::JSON)?;
        for (field, format) in [
            ("date", "date"),
            ("timestamp", "date-time"),
            ("email", "email"),
            ("uri", "uri"),
            ("uuid", "uuid"),
        ] {
            assert_eq!(schema["properties"][field]["format"], format);
        }
        assert_eq!(
            schema["properties"]["email"]["type"],
            serde_json::json!(["string", "null"])
        );

        // Schema declarations leave value validation to the host, just as
        // unformatted strings do; deserialization does not normalize data.
        let unvalidated = serde_json::json!({
            "date": "provider-date",
            "timestamp": "provider-timestamp",
            "email": null,
            "uri": "provider-uri",
            "uuid": "provider-id",
        });
        let value: FormattedStrings = serde_json::from_value(unvalidated.clone())?;
        assert_eq!(serde_json::to_value(value)?, unvalidated);
        Ok(())
    }

    #[derive(Debug, Deserialize, Serialize, crate::Schema)]
    #[schema(crate = "crate")]
    struct FormattedArrays {
        #[schema(items(format = "email"), min_items = 1, max_items = 100)]
        recipients: Vec<String>,
        #[schema(items(format = "uri"))]
        links: Option<Vec<Option<String>>>,
        #[schema(items(format = "uuid"))]
        identifiers: [String; 2],
    }

    #[test]
    fn array_formats_preserve_item_types_and_container_bounds() -> serde_json::Result<()> {
        let schema: serde_json::Value = serde_json::from_str(FormattedArrays::JSON)?;
        assert_eq!(
            schema["properties"]["recipients"],
            serde_json::json!({
                "type": "array",
                "items": {"type": "string", "format": "email"},
                "minItems": 1,
                "maxItems": 100,
            })
        );
        assert_eq!(
            schema["properties"]["links"],
            serde_json::json!({
                "type": ["array", "null"],
                "items": {"type": ["string", "null"], "format": "uri"},
            })
        );
        assert_eq!(
            schema["properties"]["identifiers"],
            serde_json::json!({
                "type": "array",
                "items": {"type": "string", "format": "uuid"},
                "minItems": 2,
                "maxItems": 2,
            })
        );
        let wire = serde_json::json!({
            "recipients": ["First.Last+Tag@EXAMPLE.com"],
            "links": ["https://EXAMPLE.com:443/a/../b?q=%2f", null],
            "identifiers": ["A", "B"],
        });
        let value: FormattedArrays = serde_json::from_value(wire.clone())?;
        assert_eq!(serde_json::to_value(value)?, wire);
        Ok(())
    }

    #[test]
    fn maximum_action_count_reuses_resource_and_connection_declarations() {
        // Evaluate at compile time: this supported action count previously
        // exceeded rustc's default const-evaluation work limit.
        static PARTS: SchemaBuffer = {
            const NAMES: [&str; 64] = [
                "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10", "a11", "a12", "a13", "a14", "a15",
                "a16", "a17", "a18", "a19", "a20", "a21", "a22", "a23", "a24", "a25", "a26", "a27", "a28", "a29",
                "a30", "a31", "a32", "a33", "a34", "a35", "a36", "a37", "a38", "a39", "a40", "a41", "a42", "a43",
                "a44", "a45", "a46", "a47", "a48", "a49", "a50", "a51", "a52", "a53", "a54", "a55", "a56", "a57",
                "a58", "a59", "a60", "a61", "a62", "a63",
            ];
            let mut actions = [const {
                ActionParts {
                    name: "",
                    resources: &[ResourceUse {
                        name: "records",
                        part: r#"{"kind":"resource","name":"records","key":"id","schema":{"type":"object","properties":{"id":{"type":"string","minLength":1,"maxLength":512},"kind":{"type":"string","minLength":1,"maxLength":64},"data":{"type":"string","format":"source","maxBytes":33554432}},"required":["id","kind","data"],"additionalProperties":false}}"#,
                        write: true,
                    }],
                    connections: &[ConnectionUse {
                        name: "provider",
                        part: r#"{"kind":"connection","name":"provider","profile":"acme.provider","scopes":["objects.read","objects.write","settings.read","settings.write"]}"#,
                    }],
                }
            }; 64];
            let mut index = 0;
            while index < actions.len() {
                actions[index].name = NAMES[index];
                index += 1;
            }
            let mut references = [&actions[0]; 64];
            let mut index = 0;
            while index < actions.len() {
                references[index] = &actions[index];
                index += 1;
            }
            extension_parts("acme.large", "1.0.0", None, "Many actions", None, &references)
        };
        let parts = PARTS.as_str();
        assert_eq!(parts.matches(r#""kind":"resource""#).count(), 1);
        assert_eq!(parts.matches(r#""kind":"connection""#).count(), 1);
        assert!(parts.contains(r#""a0","a1""#));
        assert!(parts.contains(r#""a62","a63""#));
    }

    #[test]
    fn shared_declarations_retain_first_occurrence_order_and_exact_bytes() {
        const FIRST: ActionParts = ActionParts {
            name: "first",
            resources: &[
                ResourceUse {
                    name: "a",
                    part: "resource a",
                    write: false,
                },
                ResourceUse {
                    name: "a",
                    part: "resource a",
                    write: true,
                },
            ],
            connections: &[ConnectionUse {
                name: "x",
                part: "connection x",
            }],
        };
        const SECOND: ActionParts = ActionParts {
            name: "second",
            resources: &[
                ResourceUse {
                    name: "b",
                    part: "resource b",
                    write: true,
                },
                ResourceUse {
                    name: "a",
                    part: "resource a",
                    write: true,
                },
            ],
            connections: &[
                ConnectionUse {
                    name: "y",
                    part: "connection y",
                },
                ConnectionUse {
                    name: "x",
                    part: "connection x",
                },
            ],
        };
        let parts = extension_parts(
            "acme.example",
            "1.0.0",
            Some("Example"),
            "Example",
            None,
            &[&FIRST, &SECOND],
        );
        assert_eq!(
            parts.as_str(),
            r#"{"kind":"extension","name":"acme.example","version":"1.0.0","description":"Example","label":"Example","actions":["first","second"]}resource aconnection xresource bconnection y"#
        );
    }

    #[test]
    #[should_panic(expected = "Conflicting resource declarations use the same name.")]
    fn repeated_resources_reject_a_later_conflicting_declaration() {
        const FIRST: ActionParts = ActionParts {
            name: "first",
            resources: &[ResourceUse {
                name: "a",
                part: "schema 1",
                write: true,
            }],
            connections: &[],
        };
        const CONFLICT: ActionParts = ActionParts {
            name: "conflict",
            resources: &[ResourceUse {
                name: "a",
                part: "schema 2",
                write: true,
            }],
            connections: &[],
        };
        let _ = extension_parts(
            "acme.example",
            "1.0.0",
            None,
            "Example",
            None,
            &[&FIRST, &FIRST, &CONFLICT],
        );
    }

    #[test]
    #[should_panic(expected = "Conflicting connection declarations use the same name.")]
    fn repeated_connections_reject_a_later_conflicting_declaration() {
        const FIRST: ActionParts = ActionParts {
            name: "first",
            resources: &[],
            connections: &[ConnectionUse {
                name: "a",
                part: "scopes 1",
            }],
        };
        const CONFLICT: ActionParts = ActionParts {
            name: "conflict",
            resources: &[],
            connections: &[ConnectionUse {
                name: "a",
                part: "scopes 2",
            }],
        };
        let _ = extension_parts(
            "acme.example",
            "1.0.0",
            None,
            "Example",
            None,
            &[&FIRST, &FIRST, &CONFLICT],
        );
    }
}
