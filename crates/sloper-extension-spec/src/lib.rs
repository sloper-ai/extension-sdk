#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]
//! Shared extension manifest types, validation, JSON Schema, and canonical WIT.

mod error;
mod wit;
pub use error::Error;
pub use wit::{
    WIT_PACKAGES,
    WIT_WORLD,
    write_wit,
};
mod manifest;
mod schema;
mod strict_json;

pub use manifest::{
    Action,
    Capability,
    Connection,
    Finding,
    Manifest,
    ManifestError,
    Mode,
    Resource,
};
pub use schema::Schema;
pub use strict_json::{
    parse_fragments,
    parse_object,
};

mod source;
pub use source::{
    SourceFilenameError,
    SourcePropertyError,
    ZipDirectory,
    ZipEntry,
    ZipMediaProbe,
    detect_media_type,
    source_media_matches,
    validate_source_filename,
    validate_source_property,
    zip_directory,
    zip_entry,
};

#[cfg(test)]
mod tests {

    use serde_json::{
        Value,
        json,
    };

    use crate::{
        Manifest,
        ManifestError,
        Schema,
        parse_fragments,
        parse_object,
    };

    const VALID: &[u8] = include_bytes!("../tests/testdata/extension/valid.json");

    macro_rules! assert_ok {
        ($result:expr $(,)?) => {
            match $result {
                Ok(value) => value,
                Err(error) => panic!("expected Ok, got {error:?}"),
            }
        };
    }

    macro_rules! assert_err {
        ($result:expr $(,)?) => {
            match $result {
                Ok(value) => panic!("expected Err, got {value:?}"),
                Err(error) => error,
            }
        };
    }

    fn manifest_with_property(property: Value) -> Value {
        let mut manifest: Value = assert_ok!(serde_json::from_slice(VALID));
        manifest["resources"]["invoices"]["schema"]["properties"]["document"] = property;
        manifest
    }

    fn parse_manifest(value: &Value) -> Result<Manifest, ManifestError> {
        Manifest::parse(&assert_ok!(serde_json::to_vec(value)))
    }

    #[test]
    fn shared_manifest_accept_reject_corpus() {
        let corpus: Value = serde_json::from_slice(include_bytes!("../tests/testdata/extension/corpus.json")).unwrap();
        for case in corpus.as_array().unwrap() {
            let bytes = serde_json::to_vec(&case["manifest"]).unwrap();
            let result = Manifest::parse(&bytes);
            if let Some(code) = case["code"].as_str() {
                assert_eq!(result.unwrap_err().findings()[0].code, code, "case {}", case["name"]);
            } else {
                result.unwrap();
            }
        }
    }
    #[test]
    fn duplicate_keys_are_rejected_at_every_depth() {
        for input in [
            br#"{"actions":{},"actions":{}}"#.as_slice(),
            br#"{"a":{"type":"string","type":"object"}}"#,
        ] {
            assert_eq!(
                Manifest::parse(input).unwrap_err().findings()[0].code,
                "MANIFEST_MALFORMED"
            );
        }
    }
    #[test]
    fn schemas_preserve_property_order_and_enforce_instances() {
        let schema: Schema = serde_json::from_str(
        r#"{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"integer","minimum":0}},"required":["z"]}"#,
    )
    .unwrap();
        assert_eq!(
            schema.as_value()["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["z", "a"]
        );
        schema.validate_instance(&json!({"z":"hello","a":2})).unwrap();
        assert!(schema.validate_instance(&json!({"z":"hello","extra":1})).is_err());
        assert!(schema.validate_instance(&json!({"z":"hello","a":-1})).is_err());
        assert!(schema.validate_instance(&json!({"a":1})).is_err());
        let invalid = Schema(json!({"type":"string","format":"duration"}));
        assert!(invalid.validate_instance(&json!("P1D")).is_err());
    }
    #[test]
    fn malformed_schema_types_and_constraints_are_rejected() {
        for property in [
            json!({"type":["string",12]}),
            json!({"type":["string","integer"]}),
            json!({"type":"number","maxLength":1}),
            json!({"type":"string","minimum":1}),
            json!({"type":"string","minLength":10,"maxLength":1}),
            json!({"type":"string","format":"source","maxBytes":104_857_601}),
            json!({"type":"string","maxBytes":1}),
            json!({"type":"string","enum":[1]}),
            json!({"type":"object","properties":{},"required":["absent"]}),
        ] {
            let mut manifest: Value = serde_json::from_slice(VALID).unwrap();
            manifest["resources"]["invoices"]["schema"]["properties"]["document"] = property;
            assert!(
                Manifest::parse(&serde_json::to_vec(&manifest).unwrap()).is_err(),
                "manifest {manifest}"
            );
        }
    }

    // Bound both the byte stream and the number of declarations.
    #[test]
    fn fragments_are_bounded_before_collecting_a_declaration_batch() {
        const MAX_DECLARATIONS: usize = 1 + 64 + 32 + 8;
        let valid = "{} ".repeat(MAX_DECLARATIONS);
        assert_eq!(assert_ok!(parse_fragments(valid.as_bytes())).len(), MAX_DECLARATIONS);
        let too_many = "{} ".repeat(MAX_DECLARATIONS + 1);
        assert_eq!(
            assert_err!(parse_fragments(too_many.as_bytes())).findings()[0].code,
            "EXTENSION_PARTS_INVALID",
        );
        for bytes in [
            br#"{"entry":{"id":1,"id":2}}"#.as_slice(),
            b"\xff",
            br#"{} {"unfinished": "#,
        ] {
            assert_eq!(
                assert_err!(parse_fragments(bytes)).findings()[0].code,
                "EXTENSION_PARTS_INVALID",
            );
        }
        assert_err!(parse_fragments(&vec![b' '; 256 * 1024 + 1]));
    }

    #[test]
    fn action_descriptions_are_optional_bounded_and_safe_to_display() {
        let mut manifest: Value = assert_ok!(serde_json::from_slice(VALID));
        assert!(
            assert_ok!(parse_manifest(&manifest)).actions["inspect"]
                .description
                .is_none()
        );
        manifest["actions"]["inspect"]["description"] = json!("Inspect invoice documents.");
        assert_eq!(
            assert_ok!(parse_manifest(&manifest)).actions["inspect"]
                .description
                .as_deref(),
            Some("Inspect invoice documents."),
        );
        for description in [
            String::new(),
            "x".repeat(201),
            "unsafe\ntext".into(),
            "unsafe\u{202e}text".into(),
        ] {
            manifest["actions"]["inspect"]["description"] = json!(description);
            let error = assert_err!(parse_manifest(&manifest));
            assert_eq!(error.findings()[0].code, "MANIFEST_INVALID_VALUE");
            assert_eq!(error.findings()[0].location, "/actions/inspect/description");
        }
    }

    #[test]
    fn manifest_size_is_enforced_after_direct_construction() {
        const MAX_BYTES: usize = 256 * 1024;
        let mut manifest = assert_ok!(Manifest::parse(VALID));
        manifest
            .connections
            .get_mut("ledger")
            .expect("fixture connection")
            .scopes = vec!["x".repeat(MAX_BYTES)];
        assert_eq!(
            assert_err!(manifest.validate()).findings()[0].code,
            "MANIFEST_TOO_LARGE"
        );
        let mut exact = VALID.to_vec();
        exact.resize(MAX_BYTES, b' ');
        assert_ok!(Manifest::parse(&exact));
        exact.push(b' ');
        assert_eq!(
            assert_err!(Manifest::parse(&exact)).findings()[0].code,
            "MANIFEST_TOO_LARGE"
        );
    }

    // The manifest vocabulary excludes schema defaults.
    #[test]
    fn unsupported_defaults_and_impossible_writable_keys_are_rejected() {
        let value = manifest_with_property(json!({"type":"string","default":"text"}));
        assert_eq!(
            assert_err!(parse_manifest(&value)).findings()[0].code,
            "MANIFEST_INVALID_VALUE"
        );
        let mut manifest: Value = assert_ok!(serde_json::from_slice(VALID));
        manifest["resources"]["invoices"]["schema"]["properties"]["id"]["maxLength"] = json!(0);
        assert_eq!(
            assert_err!(parse_manifest(&manifest)).findings()[0].code,
            "MANIFEST_RESOURCE_KEY_INVALID",
        );
    }

    // JSON Schema equality is numeric and independent of object property order.
    #[test]
    fn enums_and_unique_arrays_use_exact_json_value_equality() {
        for values in [
            json!([1, 1.0]),
            json!([0, -0.0]),
            json!([1_000, 1.0e3]),
            json!([{"a":1,"b":2}, {"b":2.0,"a":1.0}]),
        ] {
            let array = Schema(json!({"type":"array","uniqueItems":true}));
            assert_err!(array.validate_instance(&values));
        }
        for values in [
            json!([1, 1.5]),
            json!([9_007_199_254_740_992_u64, 9_007_199_254_740_993_u64]),
        ] {
            let array = Schema(json!({"type":"array","uniqueItems":true}));
            assert_ok!(array.validate_instance(&values));
        }
        let number = Schema(json!({"type":"number","enum":[1]}));
        assert_ok!(number.validate_instance(&json!(1.0)));
        for property in [
            json!({"type":"number","enum":[1,1.0]}),
            json!({"type":"number","minimum":2,"enum":[1]}),
            json!({"type":"string","format":"date","enum":["2026-02-30"]}),
            json!({"type":"string","enum":["hidden\u{202e}value"]}),
        ] {
            assert_eq!(
                assert_err!(parse_manifest(&manifest_with_property(property))).findings()[0].code,
                "MANIFEST_INVALID_VALUE",
            );
        }
    }

    // Formats validate exact spellings without URL or UUID normalization.
    #[test]
    fn scalar_formats_reject_invalid_spellings_and_preserve_valid_forms() {
        for (format, accepted, rejected) in [
            (
                "uuid",
                "123e4567-e89b-12d3-a456-426614174000",
                "123e4567e89b12d3a456426614174000",
            ),
            ("uri", "https://example.com/a%20b", "https://example.com/a b"),
            ("uri", "urn:example:document", "https://example.com/%zz"),
            ("email", "person+tag@example.com", "a..b@example.com"),
            ("email", "person@localhost", "person@-example.com"),
            ("email", "\"quoted local\"@example.com", "person@example..com"),
            ("email", "person@[IPv6:2001:db8::1]", "person@example.com\0"),
            ("date", "2024-02-29", "2026-02-29"),
            ("date-time", "2026-09-06T01:02:03-07:00", "2026-09-06T01:02:03"),
        ] {
            let schema = Schema(json!({"type":"string","format":format}));
            assert_ok!(schema.validate_instance(&json!(accepted)));
            assert_err!(schema.validate_instance(&json!(rejected)));
        }
    }

    // Source constraints apply in both directions, including case-insensitive MIME
    // types.
    #[test]
    fn source_bounds_and_media_type_sets_are_validated() {
        for property in [
            json!({"type":["string","null"],"format":"source"}),
            json!({"type":"string","format":"source","maxBytes":1}),
            json!({"type":"string","format":"source","maxBytes":104_857_600,"mediaTypes":["IMAGE/PNG"]}),
        ] {
            assert_ok!(parse_manifest(&manifest_with_property(property)));
        }
        for property in [
            json!({"type":"string","format":"source","maxBytes":0}),
            json!({"type":"string","format":"source","maxBytes":104_857_601}),
            json!({"type":"string","format":"source","mediaTypes":[]}),
            json!({"type":"string","format":"source","mediaTypes":["image/png","IMAGE/PNG"]}),
            json!({"type":"string","format":"source","mediaTypes":["image/png; charset=utf8"]}),
            json!({"type":"string","mediaTypes":["image/png"]}),
        ] {
            assert_err!(parse_manifest(&manifest_with_property(property)));
        }
    }

    #[test]
    fn untyped_instance_values_still_obey_host_bounds() {
        let schema = Schema(json!({"type":"object","additionalProperties":true}));
        let mut nested = json!(null);
        for _ in 0..32 {
            nested = json!([nested]);
        }
        assert_err!(schema.validate_instance(&json!({"deep":nested})));
        assert_err!(schema.validate_instance(&json!({"large":"x".repeat(8 * 1024 * 1024)})));
        assert_ok!(schema.validate_instance(&json!({"small":[1,2,3]})));
        assert_err!(parse_object(br#"{"nested":{"id":1,"id":2}}"#));
    }

    #[test]
    fn schema_depth_limits_include_all_recursive_schema_positions() {
        for keyword in ["properties", "items", "additionalProperties"] {
            let mut schema = json!({"type":"string"});
            let mut instance = json!("leaf");
            for _ in 0..32 {
                (schema, instance) = match keyword {
                    "properties" => {
                        (
                            json!({"type":"object","properties":{"child":schema}}),
                            json!({"child":instance}),
                        )
                    },
                    "items" => (json!({"type":"array","items":schema}), json!([instance])),
                    _ => {
                        (
                            json!({"type":"object","additionalProperties":schema}),
                            json!({"child":instance}),
                        )
                    },
                };
            }
            assert_ok!(Schema(schema.clone()).validate_instance(&instance));
            let too_deep = Schema(json!({"type":"array","items":schema}));
            assert_eq!(
                assert_err!(too_deep.validate_instance(&json!([instance]))).findings()[0].code,
                "MANIFEST_INVALID_VALUE",
            );
        }
    }

    #[test]
    fn schema_node_and_counted_string_limits_are_inclusive() {
        // Root, type, properties, then 256 schemas with three nodes and enum values.
        let mut properties = serde_json::Map::new();
        for index in 0..256 {
            let enum_size = if index == 255 {
                10
            } else {
                13
            };
            let values = (0..enum_size).map(|value| format!("v{value}")).collect::<Vec<_>>();
            properties.insert(format!("p{index}"), json!({"type":"string","enum":values}));
        }
        let mut schema = Schema(json!({"type":"object","properties":properties}));
        assert_ok!(schema.validate_instance(&json!({})));
        assert_ok!(schema.0["properties"]["p255"]["enum"].as_array_mut().ok_or("enum")).push(json!("extra"));
        assert_eq!(
            assert_err!(schema.validate_instance(&json!({}))).findings()[0].code,
            "MANIFEST_INVALID_VALUE",
        );

        // The two keys and string type name consume 14 counted UTF-8 bytes.
        let value = "x".repeat(64 * 1024 - 14);
        assert_ok!(Schema(json!({"type":"string","enum":[value]})).validate_instance(&json!(value)));
        let value = format!("{value}x");
        assert_eq!(
            assert_err!(Schema(json!({"type":"string","enum":[value]})).validate_instance(&json!(value))).findings()[0]
                .code,
            "MANIFEST_INVALID_VALUE",
        );
    }

    #[test]
    fn schema_property_and_enum_counts_are_bounded() {
        let mut properties = serde_json::Map::new();
        for index in 0..256 {
            properties.insert(format!("p{index}"), json!({"type":"string"}));
        }
        assert_ok!(Schema(json!({"type":"object","properties":properties})).validate_instance(&json!({})));
        properties.insert("extra".into(), json!({"type":"string"}));
        assert_err!(Schema(json!({"type":"object","properties":properties})).validate_instance(&json!({})));

        let mut values = (0..256).map(Value::from).collect::<Vec<_>>();
        assert_ok!(Schema(json!({"type":"integer","enum":values})).validate_instance(&json!(0)));
        values.push(json!(256));
        assert_err!(Schema(json!({"type":"integer","enum":values})).validate_instance(&json!(0)));
    }

    #[test]
    fn configuration_rejects_open_objects_and_sources_at_any_depth() {
        for schema in [
            json!({"type":"object","additionalProperties":true}),
            json!({"type":"object","properties":{"nested":{"type":"object","additionalProperties":{"type":"string"}}}}),
            json!({"type":"object","properties":{"nested":{"type":"array","items":{"type":"string","format":"source"}}}}),
        ] {
            let mut manifest: Value = assert_ok!(serde_json::from_slice(VALID));
            manifest["configuration"] = schema;
            assert_err!(parse_manifest(&manifest));
        }
        let mut manifest: Value = assert_ok!(serde_json::from_slice(VALID));
        manifest["configuration"] = json!({"type":"object","properties":{"nested":{"type":["object","null"],"properties":{"text":{"type":"string"}}}}});
        assert_ok!(parse_manifest(&manifest));
    }
}
