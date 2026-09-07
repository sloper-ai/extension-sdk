use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{
        BTreeSet,
        HashSet,
        hash_map::RandomState,
    },
    hash::{
        BuildHasher,
        Hash,
        Hasher,
    },
    mem,
    net::{
        Ipv4Addr,
        Ipv6Addr,
    },
    sync::LazyLock,
};

use schemars::JsonSchema;
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::{
    Map,
    Number,
    Value,
};
use time::format_description::{
    FormatDescriptionV3,
    parse_borrowed,
    well_known::Rfc3339,
};

use super::{
    manifest::{
        ManifestError,
        Pointer,
        plain_text,
        require,
        safe_text,
    },
    strict_json::{
        MAX_HOST_OBJECT_BYTES,
        fits_encoded_limit,
    },
};

/// Bounds on schema complexity and values accepted by the host.
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 4_096;
const MAX_SCHEMA_STRING_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_PROPERTIES: usize = 256;
const MAX_ENUM_VALUES: usize = 256;
const MAX_STRING_LENGTH: u64 = 1_000_000;
const MAX_ARRAY_LENGTH: u64 = 100_000;
const MAX_SOURCE_BYTES: u64 = 104_857_600;
/// Properties, nullable pairs and enums add JSON containers around schema
/// nodes.
const MAX_SCHEMA_JSON_DEPTH: usize = MAX_SCHEMA_DEPTH * 3;

/// Ordered JSON Schema in the public bounded manifest vocabulary.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Schema(pub Value);

impl JsonSchema for Schema {
    fn schema_name() -> Cow<'static, str> {
        "Schema".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let nested = generator.subschema_for::<Self>();
        let mut properties = serde_json::json!({
            "type": {"oneOf":[{"enum":["null","boolean","object","array","number","integer","string"]},{"type":"array","minItems":2,"maxItems":2,"uniqueItems":true,"contains":{"const":"null"},"items":{"enum":["null","boolean","object","array","number","integer","string"]}}]},
            "required":{"type":"array","maxItems":MAX_SCHEMA_PROPERTIES,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z_][A-Za-z0-9_]*$"}},
            "enum":{"type":"array","minItems":1,"maxItems":256,"uniqueItems":true},
            "title":{"type":"string","minLength":1,"maxLength":60}, "description":{"type":"string","minLength":1,"maxLength":500},
            "format":{"enum":["date","date-time","email","uri","uuid","source"]},
            "mediaTypes":{"type":"array","minItems":1,"items":{"type":"string"},"uniqueItems":true},
            "maxBytes":{"type":"integer","minimum":1,"maximum":104_857_600},
            "uniqueItems":{"type":"boolean"}
        });
        properties["properties"] = serde_json::json!({
            "type":"object", "maxProperties":MAX_SCHEMA_PROPERTIES,
            "propertyNames":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z_][A-Za-z0-9_]*$"},
            "additionalProperties":nested
        });
        properties["items"] = nested.clone().to_value();
        properties["additionalProperties"] = serde_json::json!({"oneOf":[{"type":"boolean"},null]});
        properties["additionalProperties"]["oneOf"][1] = nested.to_value();
        for name in ["minLength", "maxLength"] {
            properties[name] = serde_json::json!({"type":"integer","minimum":0,"maximum":1_000_000});
        }
        for name in ["minItems", "maxItems"] {
            properties[name] = serde_json::json!({"type":"integer","minimum":0,"maximum":100_000});
        }
        for name in ["minimum", "maximum"] {
            properties[name] = serde_json::json!({"type":"number","minimum":-9_007_199_254_740_991_i64,"maximum":9_007_199_254_740_991_i64});
        }
        let mut schema =
            schemars::json_schema!({"type":"object","required":["type"],"additionalProperties":false,"properties":{}});
        schema.insert("properties".into(), properties);
        schema
    }
}
impl Schema {
    /// Borrows the ordered schema declaration.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Validates an instance against this schema, including scalar formats.
    ///
    /// # Errors
    /// Returns a finding when the schema or the instance is invalid.
    pub fn validate_instance(&self, value: &Value) -> Result<(), ManifestError> {
        Self::validate_value(&self.0, value)
    }

    /// Validates an instance against a borrowed schema declaration.
    ///
    /// # Errors
    /// Returns a finding when the declaration or instance is invalid.
    ///
    /// ```
    /// use serde_json::json;
    /// use sloper_extension_spec::Schema;
    /// Schema::validate_value(&json!({"type":"integer"}), &json!(12))?;
    /// # Ok::<(), sloper_extension_spec::ManifestError>(())
    /// ```
    pub fn validate_value(schema: &Value, value: &Value) -> Result<(), ManifestError> {
        let path = Pointer::root("");
        count(schema, 0, &mut Budget::default(), path)?;
        walk(schema, path, false, 0)?;
        instance_depth(value, path, 0)?;
        require(
            fits_encoded_limit(value, MAX_HOST_OBJECT_BYTES).map_err(ManifestError::json)?,
            "MANIFEST_INSTANCE_INVALID",
            "",
            "Value exceeds the host JSON size limit.",
        )?;
        instance(schema, value, path, 0)
    }
}
#[derive(Default)]
pub(super) struct Budget {
    nodes: usize,
    strings: usize,
}
const KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "title",
    "description",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "uniqueItems",
    "format",
    "mediaTypes",
    "maxBytes",
];
const TYPES: &[&str] = &["null", "boolean", "object", "array", "number", "integer", "string"];
const SAFE_NUMBER: f64 = 9_007_199_254_740_991.0;

pub(super) fn validate_schema(
    schema: &Schema,
    path: Pointer<'_>,
    configuration: bool,
    budget: &mut Budget,
) -> Result<(), ManifestError> {
    require(
        schema.0["type"].as_str() == Some("object"),
        "MANIFEST_INVALID_VALUE",
        path,
        "Root schemas must have object type.",
    )?;
    count(&schema.0, 0, budget, path)?;
    walk(&schema.0, path, configuration, 0)
}
fn count(value: &Value, depth: usize, b: &mut Budget, path: Pointer<'_>) -> Result<(), ManifestError> {
    b.nodes += 1;
    require(
        depth <= MAX_SCHEMA_JSON_DEPTH && b.nodes <= MAX_SCHEMA_NODES,
        "MANIFEST_INVALID_VALUE",
        path,
        "Schema exceeds depth or node limits.",
    )?;
    match value {
        Value::String(s) => b.strings += s.len(),
        Value::Object(o) => {
            for (k, v) in o {
                b.strings += k.len();
                count(v, depth + 1, b, path)?;
            }
        },
        Value::Array(a) => {
            for v in a {
                count(v, depth + 1, b, path)?;
            }
        },
        _ => {},
    }
    require(
        b.strings <= MAX_SCHEMA_STRING_BYTES,
        "MANIFEST_INVALID_VALUE",
        path,
        "Schema exceeds counted string byte limit.",
    )
}
fn kind(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) if TYPES.contains(&s.as_str()) => Some(s),
        Value::Array(a)
            if a.len() == 2
                && a.iter().all(|v| v.as_str().is_some_and(|s| TYPES.contains(&s)))
                && a.iter().filter(|v| v.as_str() == Some("null")).count() == 1 =>
        {
            a.iter()
                .filter_map(Value::as_str)
                .find(|s| *s != "null" && TYPES.contains(s))
        },
        _ => None,
    }
}
fn walk(value: &Value, path: Pointer<'_>, configuration: bool, depth: usize) -> Result<(), ManifestError> {
    require(
        depth <= MAX_SCHEMA_DEPTH,
        "MANIFEST_INVALID_VALUE",
        path,
        "Schema exceeds the depth limit.",
    )?;
    let o = value
        .as_object()
        .ok_or_else(|| invalid(path, "Schema nodes must be objects."))?;
    require(
        o.keys().all(|k| KEYS.contains(&k.as_str())),
        "MANIFEST_INVALID_VALUE",
        path,
        "Schema contains an unsupported keyword.",
    )?;
    let ty = o
        .get("type")
        .and_then(kind)
        .ok_or_else(|| invalid(path, "Expected one supported type or a nullable pair."))?;
    validate_keyword_types(o, ty, path)?;
    validate_children(o, ty, path, configuration, depth)?;
    validate_bounds(o, path)?;
    validate_source(o, path, configuration)?;
    validate_enum(value, o, path, depth)
}

fn validate_keyword_types(o: &Map<String, Value>, ty: &str, path: Pointer<'_>) -> Result<(), ManifestError> {
    for (keyword, expected) in [
        ("properties", "object"),
        ("required", "object"),
        ("additionalProperties", "object"),
        ("items", "array"),
        ("minItems", "array"),
        ("maxItems", "array"),
        ("uniqueItems", "array"),
        ("minLength", "string"),
        ("maxLength", "string"),
        ("format", "string"),
        ("mediaTypes", "string"),
        ("maxBytes", "string"),
    ] {
        require(
            !o.contains_key(keyword) || ty == expected,
            "MANIFEST_INVALID_VALUE",
            path.property(keyword),
            "Keyword does not fit the schema type.",
        )?;
    }
    for keyword in ["minimum", "maximum"] {
        require(
            !o.contains_key(keyword) || matches!(ty, "number" | "integer"),
            "MANIFEST_INVALID_VALUE",
            path,
            "Numeric bound requires numeric type.",
        )?;
    }
    for (key, max) in [("title", 60), ("description", 500)] {
        if let Some(v) = o.get(key) {
            require(
                v.as_str().is_some_and(|s| plain_text(s, max)),
                "MANIFEST_INVALID_VALUE",
                path.property(key),
                "Invalid rendered schema text.",
            )?;
        }
    }
    Ok(())
}

fn validate_children(
    o: &Map<String, Value>,
    ty: &str,
    path: Pointer<'_>,
    configuration: bool,
    depth: usize,
) -> Result<(), ManifestError> {
    if let Some(properties) = o.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| invalid(path, "Properties must be an object."))?;
        require(
            properties.len() <= MAX_SCHEMA_PROPERTIES,
            "MANIFEST_INVALID_VALUE",
            path,
            "Too many schema properties.",
        )?;
        for (name, schema) in properties {
            let mut bytes = name.bytes();
            require(
                name.len() <= 64
                    && bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "MANIFEST_INVALID_VALUE",
                path,
                "Invalid schema property name.",
            )?;
            walk(
                schema,
                path.property("properties").property(name),
                configuration,
                depth + 1,
            )?;
        }
    }
    if let Some(required) = o.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| invalid(path, "Required must be an array."))?;
        let mut seen = BTreeSet::new();
        for name in required {
            require(
                name.as_str().is_some_and(|n| {
                    seen.insert(n)
                        && o.get("properties")
                            .and_then(Value::as_object)
                            .is_some_and(|p| p.contains_key(n))
                }),
                "MANIFEST_INVALID_VALUE",
                path,
                "Required names must be unique declared properties.",
            )?;
        }
    }
    if let Some(additional) = o.get("additionalProperties") {
        require(
            additional.is_boolean() || additional.is_object(),
            "MANIFEST_INVALID_VALUE",
            path,
            "Additional properties must be a boolean or schema.",
        )?;
        if additional.is_object() {
            walk(
                additional,
                path.property("additionalProperties"),
                configuration,
                depth + 1,
            )?;
        }
    }
    if configuration && ty == "object" {
        require(
            o.get("additionalProperties").is_none_or(|v| v == &Value::Bool(false)),
            "MANIFEST_INVALID_VALUE",
            path,
            "Configuration objects must be closed.",
        )?;
    }
    if let Some(items) = o.get("items") {
        walk(items, path.property("items"), configuration, depth + 1)?;
    }
    Ok(())
}

fn validate_bounds(o: &Map<String, Value>, path: Pointer<'_>) -> Result<(), ManifestError> {
    bounds(o, "minLength", "maxLength", MAX_STRING_LENGTH, path)?;
    bounds(o, "minItems", "maxItems", MAX_ARRAY_LENGTH, path)?;
    for key in ["minimum", "maximum"] {
        if let Some(v) = o.get(key) {
            require(
                v.as_f64().is_some_and(|n| n.abs() <= SAFE_NUMBER),
                "MANIFEST_INVALID_VALUE",
                path,
                "Numeric bound exceeds the safe JSON range.",
            )?;
        }
    }
    require(
        o.get("minimum")
            .and_then(Value::as_f64)
            .zip(o.get("maximum").and_then(Value::as_f64))
            .is_none_or(|(a, b)| a <= b),
        "MANIFEST_INVALID_VALUE",
        path,
        "Minimum exceeds maximum.",
    )?;
    require(
        o.get("uniqueItems").is_none_or(Value::is_boolean),
        "MANIFEST_INVALID_VALUE",
        path,
        "uniqueItems must be a boolean.",
    )?;
    Ok(())
}

fn validate_source(o: &Map<String, Value>, path: Pointer<'_>, configuration: bool) -> Result<(), ManifestError> {
    if let Some(format) = o.get("format") {
        require(
            format
                .as_str()
                .is_some_and(|f| matches!(f, "date" | "date-time" | "email" | "uri" | "uuid" | "source")),
            "MANIFEST_INVALID_VALUE",
            path,
            "Unsupported scalar format.",
        )?;
    }
    let source = o.get("format").and_then(Value::as_str) == Some("source");
    require(
        !configuration || !source,
        "MANIFEST_SOURCE_MISPLACED",
        path,
        "Sources are not allowed in configuration.",
    )?;
    require(
        source || (!o.contains_key("mediaTypes") && !o.contains_key("maxBytes")),
        "MANIFEST_INVALID_VALUE",
        path,
        "Source limits require source format.",
    )?;
    if let Some(v) = o.get("maxBytes") {
        require(
            v.as_u64().is_some_and(|n| (1..=MAX_SOURCE_BYTES).contains(&n)),
            "MANIFEST_INVALID_VALUE",
            path,
            "Source byte bound exceeds the platform range.",
        )?;
    }
    if let Some(v) = o.get("mediaTypes") {
        let a = v
            .as_array()
            .ok_or_else(|| invalid(path, "Media types must be an array."))?;
        let mut seen = BTreeSet::new();
        require(
            !a.is_empty()
                && a.iter()
                    .all(|v| v.as_str().is_some_and(|s| media_type(s) && seen.insert(MediaType(s)))),
            "MANIFEST_INVALID_VALUE",
            path,
            "Media types must be a nonempty set of valid types.",
        )?;
    }
    Ok(())
}

fn validate_enum(value: &Value, o: &Map<String, Value>, path: Pointer<'_>, depth: usize) -> Result<(), ManifestError> {
    if let Some(v) = o.get("enum") {
        let a = v.as_array().ok_or_else(|| invalid(path, "Enum must be an array."))?;
        require(
            !a.is_empty() && a.len() <= MAX_ENUM_VALUES,
            "MANIFEST_INVALID_VALUE",
            path,
            "Enum size is outside its range.",
        )?;
        let mut seen = HashSet::new();
        let hash_state = RandomState::new();
        for (index, v) in a.iter().enumerate() {
            require(
                matches_type(v, &o["type"])
                    && v.as_str().is_none_or(safe_text)
                    && seen.insert(EqualityValue::new(v, &hash_state))
                    && instance_constraints(value, v, path, depth).is_ok(),
                "MANIFEST_INVALID_VALUE",
                path.property("enum").index(index),
                "Enum values must be unique, safe to display, and valid under their schema.",
            )?;
        }
    }
    Ok(())
}
fn bounds(o: &Map<String, Value>, min: &str, max: &str, ceiling: u64, path: Pointer<'_>) -> Result<(), ManifestError> {
    for k in [min, max] {
        if let Some(v) = o.get(k) {
            require(
                v.as_u64().is_some_and(|n| n <= ceiling),
                "MANIFEST_INVALID_VALUE",
                path,
                "Length or size bound is outside its range.",
            )?;
        }
    }
    require(
        o.get(min)
            .and_then(Value::as_u64)
            .zip(o.get(max).and_then(Value::as_u64))
            .is_none_or(|(a, b)| a <= b),
        "MANIFEST_INVALID_VALUE",
        path,
        "Minimum exceeds maximum.",
    )
}
fn media_type(s: &str) -> bool {
    s.split_once('/').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && !b.contains('/')
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~/".contains(&c))
    })
}
fn invalid(path: Pointer<'_>, message: &str) -> ManifestError {
    ManifestError::invalid("MANIFEST_INVALID_VALUE", path, message)
}
fn matches_type(v: &Value, t: &Value) -> bool {
    if let Some(a) = t.as_array() {
        return a.iter().any(|t| matches_type(v, t));
    }
    match t.as_str() {
        Some("null") => v.is_null(),
        Some("boolean") => v.is_boolean(),
        Some("string") => v.is_string(),
        Some("object") => v.is_object(),
        Some("array") => v.is_array(),
        Some("number") => v.is_number(),
        Some("integer") => v.as_f64().is_some_and(|n| n.fract() == 0.0),
        _ => false,
    }
}

/// Borrowed keys compare media types without copying their ASCII spelling.
struct MediaType<'a>(&'a str);

impl PartialEq for MediaType<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}
impl Eq for MediaType<'_> {}
impl PartialOrd for MediaType<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MediaType<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(other.0.bytes().map(|byte| byte.to_ascii_lowercase()))
    }
}

/// Retains only a source reference and its semantic hash, never a copied DOM.
struct EqualityValue<'a> {
    value: &'a Value,
    hash: u64,
}

impl<'a> EqualityValue<'a> {
    fn new(value: &'a Value, state: &RandomState) -> Self {
        Self {
            value,
            hash: equality_hash(value, state),
        }
    }
}
impl PartialEq for EqualityValue<'_> {
    fn eq(&self, other: &Self) -> bool {
        values_equal(self.value, other.value)
    }
}
impl Eq for EqualityValue<'_> {}
impl Hash for EqualityValue<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left == right || number_key(left) == number_key(right),
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(left, right)| values_equal(left, right))
        },
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|right| values_equal(value, right)))
        },
        _ => false,
    }
}

fn equality_hash(value: &Value, state: &RandomState) -> u64 {
    let mut hasher = state.build_hasher();
    mem::discriminant(value).hash(&mut hasher);
    match value {
        Value::Null => {},
        Value::Bool(value) => value.hash(&mut hasher),
        Value::Number(value) => number_key(value).hash(&mut hasher),
        Value::String(value) => value.hash(&mut hasher),
        Value::Array(values) => {
            values.len().hash(&mut hasher);
            for value in values {
                equality_hash(value, state).hash(&mut hasher);
            }
        },
        Value::Object(values) => {
            values.len().hash(&mut hasher);
            // Combine keyed entry hashes commutatively to ignore property order.
            // Hash collisions still compare complete borrowed values in the set.
            let mut entries = 0_u64;
            for (name, value) in values {
                let mut entry = state.build_hasher();
                name.hash(&mut entry);
                equality_hash(value, state).hash(&mut entry);
                entries = entries.wrapping_add(entry.finish());
            }
            entries.hash(&mut hasher);
        },
    }
    hasher.finish()
}

#[derive(PartialEq, Eq, Hash)]
struct NumberKey {
    digits: String,
    exponent: i32,
    negative: bool,
}

/// Normalize exact decimal digits in the formatter's sole owned buffer.
fn number_key(value: &Number) -> NumberKey {
    let mut digits = value.to_string();
    let exponent_at = digits.find('e').unwrap_or(digits.len());
    // JSON Number's finite decimal formatter has at most 309 exponent places.
    let mut exponent = digits
        .get(exponent_at + 1..)
        .map_or(0, |value| value.parse::<i32>().expect("JSON number exponent fits i32"));
    digits.truncate(exponent_at);
    let negative = digits.starts_with('-');
    if let Some(decimal) = digits.find('.') {
        exponent -= i32::try_from(digits.len() - decimal - 1).expect("JSON number coefficient fits i32");
    }
    digits.retain(|character| character != '.' && character != '-');
    let leading_zeroes = digits.len() - digits.trim_start_matches('0').len();
    digits.drain(..leading_zeroes);
    if digits.is_empty() {
        return NumberKey {
            digits,
            exponent: 0,
            negative: false,
        };
    }
    let significant = digits.trim_end_matches('0').len();
    exponent += i32::try_from(digits.len() - significant).expect("JSON number coefficient fits i32");
    digits.truncate(significant);
    NumberKey {
        digits,
        exponent,
        negative,
    }
}

fn instance_depth(value: &Value, path: Pointer<'_>, depth: usize) -> Result<(), ManifestError> {
    require(
        depth <= MAX_SCHEMA_DEPTH,
        "MANIFEST_INSTANCE_INVALID",
        path,
        "Value exceeds the nesting depth limit.",
    )?;
    match value {
        Value::Object(values) => {
            for (name, value) in values {
                instance_depth(value, path.property(name), depth + 1)?;
            }
        },
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                instance_depth(value, path.index(index), depth + 1)?;
            }
        },
        _ => {},
    }
    Ok(())
}

fn instance(schema: &Value, v: &Value, path: Pointer<'_>, depth: usize) -> Result<(), ManifestError> {
    require(
        depth <= MAX_SCHEMA_DEPTH && matches_type(v, &schema["type"]),
        "MANIFEST_INSTANCE_INVALID",
        path,
        "Value has the wrong type or exceeds depth bounds.",
    )?;
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        require(
            values.iter().any(|candidate| values_equal(candidate, v)),
            "MANIFEST_INSTANCE_INVALID",
            path,
            "Value is outside the declared enum.",
        )?;
    }
    instance_constraints(schema, v, path, depth)
}

fn instance_constraints(schema: &Value, v: &Value, path: Pointer<'_>, depth: usize) -> Result<(), ManifestError> {
    if v.is_null() {
        return Ok(());
    }
    if let Some(o) = v.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                require(
                    o.contains_key(key),
                    "MANIFEST_INSTANCE_INVALID",
                    path.property(key),
                    "Required property is missing.",
                )?;
            }
        }
        for (key, value) in o {
            if let Some(p) = schema["properties"].get(key) {
                instance(p, value, path.property(key), depth + 1)?;
            } else {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(true)) => {},
                    Some(p @ Value::Object(_)) => instance(p, value, path.property(key), depth + 1)?,
                    _ => {
                        return Err(ManifestError::invalid(
                            "MANIFEST_INSTANCE_INVALID",
                            path.property(key),
                            "Additional property is not allowed.",
                        ));
                    },
                }
            }
        }
    }
    if let Some(a) = v.as_array() {
        size(a.len(), schema, "minItems", "maxItems", path)?;
        let mut seen = HashSet::new();
        let hash_state = RandomState::new();
        for (i, v) in a.iter().enumerate() {
            if let Some(items) = schema.get("items") {
                instance(items, v, path.index(i), depth + 1)?;
            }
            require(
                schema["uniqueItems"] != true || seen.insert(EqualityValue::new(v, &hash_state)),
                "MANIFEST_INSTANCE_INVALID",
                path,
                "Array items must be unique.",
            )?;
        }
    }
    if let Some(s) = v.as_str() {
        size(s.chars().count(), schema, "minLength", "maxLength", path)?;
        if let Some(f) = schema["format"].as_str() {
            require(
                format_matches(s, f),
                "MANIFEST_INSTANCE_INVALID",
                path,
                "String does not match its format.",
            )?;
        }
    }
    if let Some(n) = v.as_f64() {
        require(
            schema["minimum"].as_f64().is_none_or(|min| n >= min)
                && schema["maximum"].as_f64().is_none_or(|max| n <= max),
            "MANIFEST_INSTANCE_INVALID",
            path,
            "Number is outside its bounds.",
        )?;
    }
    Ok(())
}
fn size(n: usize, schema: &Value, min: &str, max: &str, path: Pointer<'_>) -> Result<(), ManifestError> {
    let n = n as u64;
    require(
        schema[min].as_u64().is_none_or(|m| n >= m) && schema[max].as_u64().is_none_or(|m| n <= m),
        "MANIFEST_INSTANCE_INVALID",
        path,
        "Value size is outside its bounds.",
    )
}
fn format_matches(s: &str, format: &str) -> bool {
    match format {
        "date" => {
            static FORMAT: LazyLock<FormatDescriptionV3<'static>> =
                LazyLock::new(|| parse_borrowed::<3>("[year]-[month]-[day]").expect("fixed date format is valid"));
            s.len() == 10 && time::Date::parse(s, &*FORMAT).is_ok()
        },
        "date-time" => time::OffsetDateTime::parse(s, &Rfc3339).is_ok(),
        "email" => email_matches(s),
        "uri" => uri_matches(s),
        "uuid" => s.len() == 36 && uuid::Uuid::parse_str(s).is_ok(),
        "source" => !s.is_empty(),
        _ => false,
    }
}

fn email_matches(value: &str) -> bool {
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    // RFC 5321 mailbox, local-part and domain length limits.
    if value.len() > 254 || local.is_empty() || local.len() > 64 || domain.is_empty() || domain.len() > 255 {
        return false;
    }
    let local_valid = if let Some(quoted) = local.strip_prefix('"').and_then(|text| text.strip_suffix('"')) {
        let mut escaped = false;
        let valid = quoted.bytes().all(|byte| {
            if escaped {
                escaped = false;
                return (32..=126).contains(&byte);
            }
            if byte == b'\\' {
                escaped = true;
                return true;
            }
            (32..=126).contains(&byte) && byte != b'"'
        });
        valid && !escaped
    } else {
        local.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte))
        })
    };
    if !local_valid {
        return false;
    }
    if let Some(literal) = domain.strip_prefix('[').and_then(|text| text.strip_suffix(']')) {
        return literal.strip_prefix("IPv6:").map_or_else(
            || literal.parse::<Ipv4Addr>().is_ok(),
            |address| address.parse::<Ipv6Addr>().is_ok(),
        );
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn uri_matches(value: &str) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            if !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !byte.is_ascii_alphanumeric() && !b"-._~:/?#[]@!$&'()*+,;=".contains(&byte) {
            return false;
        }
    }
    url::Url::parse(value).is_ok()
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;

    use serde_json::json;

    use super::{
        Pointer,
        Schema,
        equality_hash,
        format_matches,
        instance,
        values_equal,
    };

    // Borrowed equality retains exact decimal and unordered-object
    // semantics.
    #[test]
    fn borrowed_equality_preserves_numeric_and_nested_value_identity() {
        let long = "text".repeat(4096);
        let state = RandomState::new();
        for (left, right) in [
            (json!(1), json!(1.0)),
            (json!(0), json!(-0.0)),
            (json!(1000), json!(1.0e3)),
            (json!(0.000_125), json!(1.25e-4)),
            (json!(f64::MAX), json!(f64::MAX)),
            (json!(f64::MIN_POSITIVE), json!(f64::MIN_POSITIVE)),
            (
                json!({"a":[long, {"n":1}],"b":false}),
                json!({"b":false,"a":[long, {"n":1.0}]}),
            ),
        ] {
            assert!(values_equal(&left, &right));
            assert_eq!(equality_hash(&left, &state), equality_hash(&right, &state));
        }
        for (left, right) in [
            (json!(9_007_199_254_740_992_u64), json!(9_007_199_254_740_993_u64)),
            (json!(u64::MAX), json!(u64::MAX - 1)),
            (json!(1), json!(-1)),
            (json!([1, 2]), json!([2, 1])),
            (json!({"a":1}), json!({"b":1})),
            (json!([long]), json!([format!("{long}x")])),
        ] {
            assert!(!values_equal(&left, &right));
        }
    }

    // Skipping root enum membership must still enforce every other constraint.
    #[test]
    fn enum_declaration_retains_nested_constraints_and_outer_error_location() {
        for schema in [
            json!({"type":"number","minimum":2,"enum":[1]}),
            json!({"type":"string","format":"date","enum":["2026-02-30"]}),
            json!({"type":"object","properties":{"a":{"type":"integer"}},"required":["a"],"enum":[{}]}),
            json!({"type":"object","properties":{"a":{"type":"integer","enum":[1]}},"enum":[{"a":2}]}),
        ] {
            let error = Schema::validate_value(&schema, &json!(null)).unwrap_err();
            let finding = &error.findings()[0];
            assert_eq!(finding.code, "MANIFEST_INVALID_VALUE");
            assert_eq!(finding.location, "/enum/0");
            assert_eq!(
                finding.message,
                "Enum values must be unique, safe to display, and valid under their schema."
            );
        }
        let schema = json!({"type":"object","properties":{"a":{"type":"integer","enum":[1]}},"enum":[{"a":1}]});
        Schema::validate_value(&schema, &json!({"a":1.0})).unwrap();
    }

    #[test]
    fn borrowed_paths_preserve_nested_instance_and_schema_diagnostics() {
        let schema =
            Schema(json!({"type":"object","additionalProperties":{"type":"array","items":{"type":"integer"}}}));
        let error = schema
            .validate_instance(&json!({"first":[1],"~/":[1,"bad"]}))
            .unwrap_err();
        assert_eq!(error.findings()[0].location, "/~0~1/1");
        let invalid =
            Schema(json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"integer","minLength":1}}}));
        assert_eq!(
            invalid.validate_instance(&json!({})).unwrap_err().findings()[0].location,
            "/properties/b/minLength"
        );
        let mut value = json!(0);
        for _ in 0..33 {
            value = json!([value]);
        }
        assert_eq!(
            Schema(json!({"type":"array"}))
                .validate_instance(&value)
                .unwrap_err()
                .findings()[0]
                .location,
            "/0".repeat(33)
        );
    }

    #[test]
    fn large_enum_membership_compares_borrowed_candidates() {
        let values = (0..256)
            .map(|index| json!({"id":index,"text":"x".repeat(128)}))
            .collect::<Vec<_>>();
        let schema = json!({"type":"object","additionalProperties":true,"enum":values});
        instance(
            &schema,
            &json!({"text":"x".repeat(128),"id":255.0}),
            Pointer::root(""),
            0,
        )
        .unwrap();
        assert_eq!(
            instance(&schema, &json!({"text":"x".repeat(128),"id":256}), Pointer::root(""), 0)
                .unwrap_err()
                .findings()[0]
                .message,
            "Value is outside the declared enum."
        );
    }

    // MIME uniqueness folds ASCII only and retains the token grammar.
    #[test]
    fn media_type_uniqueness_borrows_case_folded_tokens() {
        for types in [
            json!(["IMAGE/PNG", "image/png"]),
            json!(["image/π"]),
            json!(["image/png; charset=utf8"]),
        ] {
            assert!(
                Schema(json!({"type":"string","format":"source","mediaTypes":types}))
                    .validate_instance(&json!("source"))
                    .is_err()
            );
        }
        Schema(json!({"type":"string","format":"source","mediaTypes":["IMAGE/PNG","image/jpeg","application/vnd.example+json"]})).validate_instance(&json!("source")).unwrap();
    }

    // The cached version-3 format keeps exact width and calendar semantics.
    #[test]
    fn cached_date_format_preserves_exact_spelling() {
        for date in ["2024-02-29", "2000-02-29", "0000-01-01", "9999-12-31"] {
            assert!(format_matches(date, "date"), "{date}");
        }
        for date in [
            "2026-02-29",
            "1900-02-29",
            "2024-2-29",
            "2024-02-9",
            "+2024-02-29",
            "-2024-02-29",
            "10000-01-01",
            "2024-02-29x",
        ] {
            assert!(!format_matches(date, "date"), "{date}");
        }
    }
}
