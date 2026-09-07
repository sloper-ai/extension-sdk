use std::{
    collections::BTreeMap,
    env::{
        self,
        VarError,
    },
    fs::File,
    io::{
        BufRead as _,
        BufReader,
        Read as _,
    },
    path::{
        Path,
        PathBuf,
    },
    sync::Arc,
};

use serde_json::{
    Map,
    Value,
    json,
};
use sloper_extension_spec::{
    Manifest,
    Schema,
    detect_media_type,
    parse_object,
    validate_source_filename,
};

use super::super::{
    Error,
    read_bounded,
    text,
};
use crate::serialized_exceeds;

// Reader item admission limit; parameters and configuration have separate
// bounds.
const MAX_ITEM_BYTES: u64 = 1024 * 1024;

pub(super) struct LocalSource {
    pub(super) filename: String,
    pub(super) media_type: &'static str,
    pub(super) bytes: Arc<[u8]>,
}

pub(super) struct Inputs {
    pub(super) action: String,
    pub(super) parameters: Value,
    pub(super) configuration: Value,
    pub(super) readers: BTreeMap<String, Vec<Value>>,
    pub(super) sources: BTreeMap<String, LocalSource>,
    pub(super) tokens: BTreeMap<String, String>,
}

impl Inputs {
    pub(super) fn parse(arguments: &mut Map<String, Value>, manifest: &Manifest) -> Result<Self, Error> {
        let action_name = text(arguments, "action")?.to_owned();
        let action = manifest
            .actions
            .get(&action_name)
            .ok_or_else(|| Error::invalid("unknown extension action"))?;
        let mut sources = BTreeMap::new();
        let mut source_ids = BTreeMap::new();
        for (property, path) in file_map(arguments, "source")? {
            let bytes = read_bounded(&path, 100 * 1024 * 1024)?;
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::usage("source filename must be UTF-8"))?
                .to_owned();
            validate_filename(&filename)?;
            let id = format!("src_{}", uuid::Uuid::new_v4().simple());
            source_ids.insert(property, id.clone());
            sources.insert(
                id,
                LocalSource {
                    filename,
                    media_type: detect_media_type(&bytes).unwrap_or("application/octet-stream"),
                    bytes: bytes.into(),
                },
            );
        }
        let mut parameters = arguments.remove("parameters").unwrap_or_else(|| json!({}));
        for (property, id) in &source_ids {
            let schema = action
                .parameters
                .as_ref()
                .and_then(|schema| schema.as_value().get("properties"))
                .and_then(|properties| properties.get(property));
            if schema.and_then(|schema| schema.get("format")).and_then(Value::as_str) == Some("source") {
                parameters
                    .as_object_mut()
                    .ok_or_else(|| Error::usage("parameters must be an object"))?
                    .insert(property.clone(), json!(id));
            }
        }
        rewrite_sources(
            action.parameters.as_ref().map(Schema::as_value),
            &mut parameters,
            &source_ids,
        )?;
        validate_object(action.parameters.as_ref(), &parameters, 8 * 1024 * 1024, "parameters")?;
        validate_sources(action.parameters.as_ref().map(Schema::as_value), &parameters, &sources)?;
        let configuration = arguments.remove("configuration").unwrap_or_else(|| json!({}));
        validate_object(
            manifest.configuration.as_ref(),
            &configuration,
            256 * 1024,
            "configuration",
        )?;
        let mut readers = super::reader_names(manifest, &action_name);
        for (name, path) in file_map(arguments, "resource")? {
            let items = readers
                .get_mut(&name)
                .ok_or_else(|| Error::invalid(format!("{name} is not a reader declared by this action")))?;
            let schema = &manifest.resources[&name].schema;
            *items = read_fixture(&path, schema, &source_ids, &sources)?;
        }
        let mut tokens = BTreeMap::new();
        for name in &action.connections {
            let variable = format!("SLOPER_TOKEN_{}", name.to_ascii_uppercase().replace('-', "_"));
            match env::var(&variable) {
                Ok(token) if !token.is_empty() && token.len() <= 16_384 => {
                    tokens.insert(name.clone(), token);
                },
                Ok(_) => {
                    return Err(Error::invalid(format!(
                        "{variable} must contain between 1 and 16384 bytes"
                    )));
                },
                Err(VarError::NotPresent) => {},
                Err(VarError::NotUnicode(_)) => {
                    return Err(Error::usage(format!("{variable} must be UTF-8")));
                },
            }
        }
        Ok(Self {
            action: action_name,
            parameters,
            configuration,
            readers,
            sources,
            tokens,
        })
    }
}

fn read_fixture(
    path: &Path,
    schema: &Schema,
    source_ids: &BTreeMap<String, String>,
    sources: &BTreeMap<String, LocalSource>,
) -> Result<Vec<Value>, Error> {
    let mut items = Vec::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let mut position = 0_u64;
    loop {
        line.clear();
        let read = reader.by_ref().take(MAX_ITEM_BYTES + 2).read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        position += 1;
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.len() as u64 > MAX_ITEM_BYTES {
            return Err(Error::invalid(format!(
                "{}:{position}: reader item exceeds 1 MiB",
                path.display()
            )));
        }
        let mut item = parse_object(&line).map_err(|_| {
            Error::usage(format!(
                "{}:{position}: expected a JSON object without duplicate keys",
                path.display()
            ))
        })?;
        rewrite_sources(Some(schema.as_value()), &mut item, source_ids)?;
        schema
            .validate_instance(&item)
            .map_err(|error| Error::validation(json!(error.findings())))?;
        validate_sources(Some(schema.as_value()), &item, sources)?;
        items.push(item);
    }
    Ok(items)
}

fn rewrite_sources(schema: Option<&Value>, value: &mut Value, ids: &BTreeMap<String, String>) -> Result<(), Error> {
    let Some(schema) = schema else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    if schema.get("format").and_then(Value::as_str) == Some("source") {
        let key = value
            .as_str()
            .ok_or_else(|| Error::invalid("source reference must be a string"))?;
        let id = ids
            .get(key)
            .or_else(|| ids.values().find(|id| id.as_str() == key))
            .ok_or_else(|| Error::invalid("source reference was not explicitly supplied"))?;
        *value = json!(id);
    }
    if let (Some(properties), Some(values)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object_mut(),
    ) {
        for (name, schema) in properties {
            if let Some(value) = values.get_mut(name) {
                rewrite_sources(Some(schema), value, ids)?;
            }
        }
    }
    if let (Some(schema), Some(values)) = (schema.get("items"), value.as_array_mut()) {
        for value in values {
            rewrite_sources(Some(schema), value, ids)?;
        }
    }
    Ok(())
}

fn validate_object(schema: Option<&Schema>, value: &Value, limit: usize, name: &str) -> Result<(), Error> {
    if serialized_exceeds(value, limit)? {
        return Err(Error::invalid(format!("{name} exceeds {limit} bytes")));
    }
    if let Some(schema) = schema {
        schema
            .validate_instance(value)
            .map_err(|error| Error::validation(json!(error.findings())))?;
    } else if value != &json!({}) {
        return Err(Error::invalid(format!("{name} is not declared")));
    }
    Ok(())
}

pub(super) fn validate_sources(
    schema: Option<&Value>,
    value: &Value,
    sources: &BTreeMap<String, LocalSource>,
) -> Result<(), Error> {
    let Some(schema) = schema else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    if schema.get("format").and_then(Value::as_str) == Some("source") {
        let source = value
            .as_str()
            .and_then(|id| sources.get(id))
            .ok_or_else(|| Error::invalid("source must be supplied explicitly"))?;
        if !source_allowed(schema, source) {
            return Err(Error::invalid("source does not satisfy its schema constraints"));
        }
    }
    if let (Some(properties), Some(values)) = (schema.get("properties").and_then(Value::as_object), value.as_object()) {
        for (name, schema) in properties {
            if let Some(value) = values.get(name) {
                validate_sources(Some(schema), value, sources)?;
            }
        }
    }
    if let (Some(schema), Some(values)) = (schema.get("items"), value.as_array()) {
        for value in values {
            validate_sources(Some(schema), value, sources)?;
        }
    }
    Ok(())
}

pub(super) fn source_allowed(schema: &Value, source: &LocalSource) -> bool {
    source.bytes.len() as u64
        <= schema
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(100 * 1024 * 1024)
        && schema
            .get("mediaTypes")
            .and_then(Value::as_array)
            .is_none_or(|types| types.iter().any(|value| value == source.media_type))
}

fn file_map(arguments: &Map<String, Value>, name: &str) -> Result<BTreeMap<String, PathBuf>, Error> {
    let mut result = BTreeMap::new();
    if let Some(entries) = arguments.get(name) {
        for entry in entries
            .as_array()
            .ok_or_else(|| Error::usage(format!("{name} must be an array")))?
        {
            let (key, path) = entry
                .as_str()
                .and_then(|entry| entry.split_once('='))
                .filter(|(key, path)| !key.is_empty() && !path.is_empty())
                .ok_or_else(|| Error::usage(format!("{name} must be NAME=PATH")))?;
            if result.insert(key.to_owned(), PathBuf::from(path)).is_some() {
                return Err(Error::usage(format!("duplicate {name} key {key}")));
            }
        }
    }
    Ok(result)
}

pub(super) fn validate_filename(filename: &str) -> Result<(), Error> {
    Ok(validate_source_filename(filename)?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn duplicate_fixture_names_are_usage_errors() {
        let arguments = json!({"resource":["invoices=a.jsonl","invoices=b.jsonl"]});
        assert!(file_map(arguments.as_object().unwrap(), "resource").is_err());
    }

    #[test]
    fn source_constraints_check_real_bytes_and_media() {
        let source = LocalSource {
            filename: "invoice.pdf".into(),
            media_type: "application/pdf",
            bytes: Arc::from(b"%PDF-1.7".as_slice()),
        };
        assert!(source_allowed(
            &json!({"maxBytes":8,"mediaTypes":["application/pdf"]}),
            &source
        ));
        assert!(!source_allowed(&json!({"maxBytes":7}), &source));
        assert!(!source_allowed(&json!({"mediaTypes":["image/png"]}), &source));
    }

    #[test]
    fn fixtures_validate_each_json_line_and_rewrite_explicit_source_keys() {
        let manifest = Manifest::parse(include_bytes!("../../tests/fixtures/valid.json")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("invoices.jsonl");
        let source = directory.path().join("invoice.pdf");
        fs::write(&source, b"%PDF-1.7").unwrap();
        fs::write(
            &fixture,
            b"{\"id\":\"one\",\"number\":\"A\",\"document\":\"receipt\"}\n",
        )
        .unwrap();
        let arguments = json!({"action":"inspect","resource":[format!("invoices={}",fixture.display())],"source":[format!("receipt={}",source.display())]});
        let inputs = Inputs::parse(&mut arguments.as_object().unwrap().clone(), &manifest).unwrap();
        let id = inputs.readers["invoices"][0]["document"].as_str().unwrap();
        assert!(inputs.sources.contains_key(id), "source={id}");
        assert_eq!(inputs.parameters, json!({}));
        fs::write(&fixture, b"{\"id\":\"one\",\"id\":\"two\",\"number\":\"A\"}\n").unwrap();
        assert!(Inputs::parse(&mut arguments.as_object().unwrap().clone(), &manifest).is_err());
        fs::write(&fixture, b"{\"id\":\"one\"}\n").unwrap();
        assert!(Inputs::parse(&mut arguments.as_object().unwrap().clone(), &manifest).is_err());
    }
    #[test]
    fn object_byte_limits_count_escaped_unicode_without_buffering() {
        let manifest = Manifest::parse(include_bytes!("../../tests/fixtures/valid.json")).unwrap();
        let schema = &manifest.resources["invoices"].schema;
        let value = json!({"id":"é","number":"quote\"\n"});
        let length = serde_json::to_vec(&value).unwrap().len();
        assert!(validate_object(Some(schema), &value, length, "parameters").is_ok());
        assert!(validate_object(Some(schema), &value, length - 1, "parameters").is_err());
    }
    #[test]
    fn parsing_moves_owned_parameter_and_configuration_payloads() {
        let mut declaration: Value = serde_json::from_slice(include_bytes!("../../tests/fixtures/valid.json")).unwrap();
        let schema = json!({"type":"object","properties":{"id":{"type":"string"},"number":{"type":"string"}},"additionalProperties":false});
        declaration["actions"]["pull"]["parameters"] = schema.clone();
        declaration["configuration"] = schema;
        let manifest = Manifest::parse(&serde_json::to_vec(&declaration).unwrap()).unwrap();
        let mut arguments = json!({"action":"pull","parameters":{"id":"one","number":"parameter allocation"},"configuration":{"id":"two","number":"configuration allocation"}}).as_object().unwrap().clone();
        let parameter = arguments["parameters"]["number"].as_str().unwrap().as_ptr();
        let configuration = arguments["configuration"]["number"].as_str().unwrap().as_ptr();
        let inputs = Inputs::parse(&mut arguments, &manifest).unwrap();
        assert_eq!(inputs.parameters["number"].as_str().unwrap().as_ptr(), parameter);
        assert_eq!(inputs.configuration["number"].as_str().unwrap().as_ptr(), configuration);
        assert!(!arguments.contains_key("parameters"));
        assert!(!arguments.contains_key("configuration"));
    }
}
